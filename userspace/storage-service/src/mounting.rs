use super::*;

pub(super) struct Mount {
    /// Its device's registry id.
    pub(super) device: u32,
    pub(super) label: String,
    pub(super) mode: Mode,
    /// A namespace of its own, with the server bound at `/`: what a `SUBNAMESPACE` reply hands on.
    pub(super) ns: u64,
    /// The server's control channel: kept, for the unmount it will carry, and waited on, since
    /// its closing means the server has gone.
    pub(super) control: u64,
    /// The server's process handle.
    pub(super) process: u64,
}

/// Spawn `path` with `control_end` moved into it; on `None` it is closed, a failed spawn moving
/// nothing.
fn spawn(root_ns: u64, path: &[u8], control_end: u64) -> Option<u64> {
    let (st, image) = lookup(root_ns, path, RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        close(control_end);
        return None;
    }
    let args = SpawnArgs {
        image,
        handle_count: 1,
        move_mask: 1, // move the control endpoint to the child
        arg0: 0,
        handles: [control_end, 0, 0, 0],
        rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
        namespace: 0,
        syscaps: 0, // a resource server holds no ambient capabilities
    };
    // SAFETY: `args` is a valid `SpawnArgs` naming handles this process holds.
    let h = unsafe { syscall1(SYS_PROCESS_SPAWN, (&args as *const SpawnArgs) as u64) };
    close(image);
    if h <= 0 {
        close(control_end);
        return None;
    }
    Some(h as u64)
}

/// Send a filesystem server its **setup message**: the device in `handles[0]`, and one flags byte.
/// Not rsproto-framed — it precedes everything else on the channel (`rsproto-wire-format.md`).
fn send_setup(control: u64, device: u64, flags: u8) -> bool {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid buffers; single-threaded.
    unsafe {
        let Some(n) = fs_setup(&mut REPLY_MSG[PAYLOAD_OFF..], flags) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = device;
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Wait, bounded, for a server's `Meta::Ready` and take its endpoint; `Err` says what came instead.
fn wait_ready(control: u64) -> Result<u64, &'static [u8]> {
    if !wait_until(control, now_ns().saturating_add(READY_TIMEOUT_NS)) {
        return Err(b"it sent no Ready in time");
    }
    let m = match recv(control) {
        Ok(Some(m)) => m,
        Ok(None) => return Err(b"what it sent was not a message"),
        Err(()) => return Err(b"it exited before Ready"),
    };
    match (m.op, m.flags & RS_FLAG_ERROR != 0, m.handles.as_slice()) {
        (librsproto::OP_READY, false, &[endpoint]) => Ok(endpoint),
        (librsproto::OP_READY, true, _) => {
            m.handles.iter().for_each(|&h| close(h));
            Err(b"it refused the device")
        }
        _ => {
            m.handles.iter().for_each(|&h| close(h));
            Err(b"it sent something other than Ready")
        }
    }
}

/// **Flush the drive's volatile write cache** (`IoOpcode::Flush`; `drivers-and-irps.md` § Flush):
/// `true` once it completes. Needs `WRITE` on the node, which the manager's duplicate carries.
pub(super) fn flush(node: u64) -> bool {
    let op = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `node` is a block device handle this process holds; `&op` is a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, node, (&op as *const IoOp) as u64) };
    po >= 0 && po_wait(po as u64).0 == 0
}

/// **Have the kernel read a disk's partition table again** (`IoOpcode::Rescan`): its partitions
/// depart and the table's are published, as arrivals read from the subscription after. How many
/// were published, or the kernel's refusal (`Unsupported` for every disk but a USB disk's).
pub(super) fn rescan(node: u64) -> Result<u64, KError> {
    let op = IoOp { opcode: IO_OPCODE_RESCAN, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `node` is a block device handle this process holds; `&op` is a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, node, (&op as *const IoOp) as u64) };
    if po < 0 {
        return Err(KError::from_i32(po as i32));
    }
    match po_wait(po as u64) {
        (0, published) => Ok(published),
        (status, _) => Err(KError::from_i32(status as i32)),
    }
}

/// **Stop a stick's medium** (`IoOpcode::Eject`); the kernel's error otherwise.
pub(super) fn eject_medium(node: u64) -> Result<(), KError> {
    let op = IoOp { opcode: IO_OPCODE_EJECT, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `node` is a block device handle this process holds; `&op` is a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, node, (&op as *const IoOp) as u64) };
    if po < 0 {
        return Err(KError::from_i32(po as i32));
    }
    match po_wait(po as u64).0 {
        0 => Ok(()),
        status => Err(KError::from_i32(status as i32)),
    }
}

/// Why a stick's medium did not stop, in the eject line's words.
pub(super) fn not_stopped(err: KError) -> &'static [u8] {
    match err {
        KError::Unsupported => b"it cannot be stopped",
        KError::IoError => b"it failed the command",
        KError::PeerClosed => b"it went away",
        _ => b"the kernel refused it",
    }
}

/// Mount `plan`'s device: spawn a server over it, and build its namespace. The server gets a
/// duplicate of `node`; this service keeps its own, for the flush and for a later mount.
pub(super) fn mount(root_ns: u64, plan: &Plan, node: u64) -> Result<Mount, &'static [u8]> {
    // **The server's handle, narrowed to the mode**: a read-only mount's server cannot write the
    // device even if it tried, whatever `ReadOnly` does above it.
    let write = if plan.mode == Mode::Rw { RIGHT_WRITE } else { 0 };
    // SAFETY: duplicating a node this process holds with DUPLICATE, narrowed.
    let device = unsafe { syscall2(SYS_HANDLE_DUPLICATE, node, RIGHT_READ | write | RIGHT_TRANSFER | RIGHT_DUPLICATE) };
    if device <= 0 {
        return Err(b"its node would not duplicate");
    }
    let device = device as u64;
    let Some((control, server_end)) = make_channel(4) else {
        close(device);
        return Err(b"no control channel");
    };
    let Some(process) = spawn(root_ns, plan.server.path(), server_end) else {
        close(device);
        close(control);
        return Err(b"its server would not spawn");
    };
    // **From here a failure lets the server go**: its control channel and handle closed. **That,
    // not the terminate, ends it**, a request neither filesystem server reads: a server that had
    // not answered `Ready` finds its control channel closed and exits; one that had exits when its
    // forwarding endpoint loses its peer. The terminate stays, for a server that does listen.
    let abandon = |control: u64| {
        close(control);
        // SAFETY: the Process handle the spawn returned, with SIGNAL.
        unsafe { syscall1(SYS_PROCESS_TERMINATE, process) };
        close(process);
    };
    let flags = if plan.mode == Mode::Ro { FS_SETUP_READ_ONLY } else { 0 };
    if !send_setup(control, device, flags) {
        close(device);
        abandon(control);
        return Err(b"the device could not be handed to its server");
    }
    let endpoint = match wait_ready(control) {
        Ok(e) => e,
        Err(why) => {
            abandon(control);
            return Err(why);
        }
    };
    // SAFETY: register-only syscall; returns a fresh namespace handle with every right.
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns <= 0 {
        close(endpoint);
        abandon(control);
        return Err(b"no namespace for it");
    }
    let ns = ns as u64;
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall4(SYS_NS_BIND, ns, b"/".as_ptr() as u64, 1, endpoint) };
    // The binding holds its own reference to the endpoint.
    close(endpoint);
    if bound != 0 {
        close(ns);
        abandon(control);
        return Err(b"its server would not bind into its namespace");
    }
    Ok(Mount { device: plan.device, label: plan.label.clone(), mode: plan.mode, ns, control, process })
}
