use super::*;

pub(super) struct Mount {
    /// Its device's registry id.
    pub(super) device: u32,
    pub(super) label: String,
    pub(super) mode: Mode,
    /// A namespace of its own, with the server bound at `/`: what a `SUBNAMESPACE` reply hands on.
    pub(super) ns: Ns,
    /// The server's control channel: kept, for the unmount it will carry, and waited on, since
    /// its closing means the server has gone.
    pub(super) control: Chan,
    /// The server's process handle.
    pub(super) process: Handle<Process, Only>,
}

/// Spawn `path` with `control_end` moved into it; a failed spawn moves nothing, and drops it.
fn spawn(root: &Root, path: &[u8], control_end: Chan) -> Option<Handle<Process, Only>> {
    let mut spawn = block_on(Spawn::path(root, core::str::from_utf8(path).ok()?)).ok()?;
    // A resource server holds no ambient capabilities, so no syscaps are given.
    spawn.grant(control_end, Rights::SEND | Rights::RECV | Rights::TRANSFER | Rights::WAIT).ok()?;
    spawn.spawn().ok()
}

/// Send a filesystem server its **setup message**: the device in `handles[0]`, and one flags byte.
/// Not rsproto-framed — it precedes everything else on the channel (`rsproto-wire-format.md`).
fn send_setup(control: &Chan, device: Node, flags: u8) -> bool {
    let mut msg = Msg::new();
    let Some(n) = fs_setup(msg.payload_mut(), flags) else {
        return false;
    };
    let mut carried = Carried::new();
    msg.set_len(n).is_ok() && carried.push(device).is_ok() && control.send(&msg, &mut carried).is_ok()
}

/// Wait, bounded, for a server's `Meta::Ready` and take its endpoint; `Err` says what came instead.
fn wait_ready(control: &Chan) -> Result<Chan, &'static [u8]> {
    if !wait_until(control, within(READY_TIMEOUT_NS)) {
        return Err(b"it sent no Ready in time");
    }
    let (m, mut sent) = match recv(control) {
        Ok(Some(m)) => m,
        Ok(None) => return Err(b"what it sent was not a message"),
        Err(()) => return Err(b"it exited before Ready"),
    };
    // A Ready's one endpoint is kept; anything else closes as `sent` drops.
    match (m.op, m.flags & RS_FLAG_ERROR != 0, sent.len()) {
        (librsproto::OP_READY, false, 1) if let Some(endpoint) = sent.take(0) => Ok(endpoint),
        (librsproto::OP_READY, true, _) => Err(b"it refused the device"),
        _ => Err(b"it sent something other than Ready"),
    }
}

/// Write back every dirty file of the mount in `ns`, held or not. `false` if one failed.
pub(super) fn sync_mount(ns: &Ns) -> bool {
    block_on(ns.flush("/")).is_ok()
}

/// **How many of the mount's files something still holds**, asked after a sync. An IRP whose
/// completion is still running on another CPU holds its file for that moment (`syscall-abi.md`),
/// so a nonzero count is asked again after a park before it is believed; a real holder still
/// holds. A nonzero count is said, naming `label`.
pub(super) fn held_files(label: &str, ns: &Ns) -> i64 {
    let mut held = 0;
    for attempt in 0..HELD_ASKS {
        if attempt > 0 {
            let _ = time::sleep(HELD_PARK_NS);
        }
        held = ns.held("/").map_or_else(|e| e.code().as_i32() as i64, |n| n as i64);
        if held == 0 {
            break;
        }
    }
    if held != 0 {
        Line::new()
            .s(b"storage-service: ")
            .untrusted(label.as_bytes())
            .s(b" is in use: ")
            .i(held)
            .s(b" file(s) still open or mapped")
            .end();
    }
    held
}

/// Run `opcode` on `node` and wait for it: its completion, or why the submit or the wait failed.
fn run(node: &Node, opcode: u32) -> Result<IoResult, KError> {
    let op = IoOp { opcode, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    Op::submit(node.raw(), &op).and_then(Op::wait_out).map_err(|e| e.code())
}

/// **Flush the drive's volatile write cache** (`IoOpcode::Flush`; `drivers-and-irps.md` § Flush):
/// `true` once it completes. Needs `WRITE` on the node, which the manager's duplicate carries.
pub(super) fn flush(node: &Node) -> bool {
    run(node, IO_OPCODE_FLUSH).is_ok_and(|done| done.status == 0)
}

/// **Have the kernel read a disk's partition table again** (`IoOpcode::Rescan`): its partitions
/// depart and the table's are published, as arrivals read from the subscription after. How many
/// were published, or the kernel's refusal (`Unsupported` for every disk but a USB disk's).
pub(super) fn rescan(node: &Node) -> Result<u64, KError> {
    match run(node, IO_OPCODE_RESCAN)? {
        IoResult { status: 0, result, .. } => Ok(result),
        IoResult { status, .. } => Err(KError::from_i32(status)),
    }
}

/// **Stop a stick's medium** (`IoOpcode::Eject`); the kernel's error otherwise.
pub(super) fn eject_medium(node: &Node) -> Result<(), KError> {
    match run(node, IO_OPCODE_EJECT)?.status {
        0 => Ok(()),
        status => Err(KError::from_i32(status)),
    }
}

/// Let a server that did not come up go: its control channel closed, **which, not the terminate,
/// ends it**, a request neither filesystem server reads. A server that had not answered `Ready`
/// finds its control channel closed and exits; one that had exits when its forwarding endpoint
/// loses its peer. The terminate stays, for a server that does listen.
fn abandon(control: Chan, process: Handle<Process, Only>) {
    drop(control);
    let _ = process.terminate();
}

/// Mount `plan`'s device: spawn a server over it, and build its namespace. The server gets a
/// duplicate of `node`; this service keeps its own, for the flush and for a later mount.
pub(super) fn mount(root: &Root, plan: &Plan, node: &Node) -> Result<Mount, &'static [u8]> {
    // **The server's handle, narrowed to the mode**: a read-only mount's server cannot write the
    // device even if it tried, whatever `ReadOnly` does above it.
    let write = if plan.mode == Mode::Rw { Rights::WRITE } else { Rights::empty() };
    let Ok(device) = node.duplicate(Rights::READ | write | Rights::TRANSFER | Rights::DUPLICATE) else {
        return Err(b"its node would not duplicate");
    };
    let Ok((control, server_end)) = Chan::pair(4) else {
        return Err(b"no control channel");
    };
    let Some(process) = spawn(root, plan.server.path(), server_end) else {
        return Err(b"its server would not spawn");
    };
    // **From here a failure lets the server go** ([`abandon`]).
    let flags = if plan.mode == Mode::Ro { FS_SETUP_READ_ONLY } else { 0 };
    if !send_setup(&control, device, flags) {
        abandon(control, process);
        return Err(b"the device could not be handed to its server");
    }
    let endpoint = match wait_ready(&control) {
        Ok(e) => e,
        Err(why) => {
            abandon(control, process);
            return Err(why);
        }
    };
    let Ok(ns) = Ns::create() else {
        drop(endpoint);
        abandon(control, process);
        return Err(b"no namespace for it");
    };
    let bound = ns.bind_server("/", endpoint.as_ref(), "");
    // The binding holds its own reference to the endpoint.
    drop(endpoint);
    if bound.is_err() {
        drop(ns);
        abandon(control, process);
        return Err(b"its server would not bind into its namespace");
    }
    Ok(Mount { device: plan.device, label: plan.label.clone(), mode: plan.mode, ns, control, process })
}
