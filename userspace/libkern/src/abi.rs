//! `#[repr(C)]` boundary types — the userspace mirror of the kernel ABI structs.
//!
//! Mirrors `kernel/src/libkern/{spawn,thread,notification,ipc,io_result}.rs` and
//! the `HandleInfo` in `kernel/src/libkern/handle.rs`. Each layout carries the
//! same compile-time `offset_of!`/`size_of` asserts the kernel uses, so the two
//! sides self-pin to identical layouts until `cargo xtask abi-sync-check` lands.
//!
//! Inline handle arrays use `u64` (the raw handle bits) rather than `RawHandle`
//! for ergonomics at raw-syscall call sites; `RawHandle` is `repr(transparent)`
//! over `u64`, so the layout is identical.

use core::mem::{align_of, offset_of, size_of};

// --- sys_process_spawn -----------------------------------------------------

/// Maximum initial handles a parent can install in a child at spawn.
pub const SPAWN_MAX_HANDLES: usize = 4;

/// The spawn argument block, passed by pointer to `sys_process_spawn`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SpawnArgs {
    /// A `MemoryObject` handle holding the program's ELF image (offset 0). The spawner
    /// resolves the executable path (`sys_ns_lookup` → a readable object) and passes the
    /// handle, which `sys_process_spawn` reads (requires `MAP_READ`) and loads.
    pub image: u64,
    /// Valid entries in `handles`/`rights`; `≤ SPAWN_MAX_HANDLES` (offset 8).
    pub handle_count: u32,
    /// Bit `i` set ⇒ **move** `handles[i]` to the child; clear ⇒ duplicate (offset 12).
    pub move_mask: u32,
    /// Opaque user data handed to the child at entry (in `rcx`) (offset 16).
    pub arg0: u64,
    /// Parent-side handles to install in the child's table (offset 24).
    pub handles: [u64; SPAWN_MAX_HANDLES],
    /// Per-handle rights attenuation bound; installed = `source & rights[i]` (offset 56).
    pub rights: [u64; SPAWN_MAX_HANDLES],
    /// Child's root namespace; `0` ⇒ inherit a LOOKUP-only handle to the parent's
    /// namespace, non-null ⇒ a (restricted) namespace the parent holds (offset 88).
    pub namespace: u64,
    /// Ambient [`SysCaps`](crate::syscaps::SysCaps) to grant the child, raw bits
    /// (offset 96). The kernel installs `parent.syscaps & syscaps` (⊆-parent). `0` ⇒
    /// unprivileged. See `docs/architecture/syscaps.md`.
    pub syscaps: u64,
}

const _: () = assert!(size_of::<SpawnArgs>() == 104);
const _: () = assert!(align_of::<SpawnArgs>() == 8);
const _: () = assert!(offset_of!(SpawnArgs, image) == 0);
const _: () = assert!(offset_of!(SpawnArgs, handle_count) == 8);
const _: () = assert!(offset_of!(SpawnArgs, move_mask) == 12);
const _: () = assert!(offset_of!(SpawnArgs, arg0) == 16);
const _: () = assert!(offset_of!(SpawnArgs, handles) == 24);
const _: () = assert!(offset_of!(SpawnArgs, rights) == 56);
const _: () = assert!(offset_of!(SpawnArgs, namespace) == 88);
const _: () = assert!(offset_of!(SpawnArgs, syscaps) == 96);

// --- sys_thread_create / sys_thread_get_registers --------------------------

/// The argument block `sys_thread_create` reads to start a new thread.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ThreadArgs {
    /// Ring-3 entry point VA (offset 0).
    pub entry: u64,
    /// Initial user stack pointer VA — the stack top (offset 8).
    pub user_sp: u64,
    /// Opaque bootstrap word, delivered to the thread in `rdx` (offset 16).
    pub arg0: u64,
    /// Scheduling class: `THREAD_CLASS_TIMESHARED` (`0`, default) or
    /// `THREAD_CLASS_REALTIME` (`1`; requires the `REAL_TIME` syscap) (offset 24).
    pub class: u8,
    /// RealTime fixed priority `0..=99`; ignored for TimeShared (offset 25).
    pub rt_priority: u8,
    /// TimeShared `nice` `-20..=19`; ignored for RealTime (offset 26).
    pub nice: i8,
    /// CPU affinity mask; `0` ⇒ no restriction (offset 27).
    pub cpu_affinity: u8,
    /// Reserved; must be zero (offset 28).
    pub _reserved: [u8; 36],
}

/// `ThreadArgs::class` — the default fair/cooperative class (a zeroed block).
pub const THREAD_CLASS_TIMESHARED: u8 = 0;
/// `ThreadArgs::class` — fixed-priority real-time (requires the `REAL_TIME` syscap).
pub const THREAD_CLASS_REALTIME: u8 = 1;

const _: () = assert!(size_of::<ThreadArgs>() == 64);
const _: () = assert!(align_of::<ThreadArgs>() == 8);
const _: () = assert!(offset_of!(ThreadArgs, entry) == 0);
const _: () = assert!(offset_of!(ThreadArgs, user_sp) == 8);
const _: () = assert!(offset_of!(ThreadArgs, arg0) == 16);
const _: () = assert!(offset_of!(ThreadArgs, class) == 24);
const _: () = assert!(offset_of!(ThreadArgs, cpu_affinity) == 27);

/// The faulted-register snapshot `sys_thread_get_registers` writes: the 16 GPRs
/// (incl. `rsp`), then `rip` (index 16) and `rflags` (index 17).
#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct RegisterValues {
    pub regs: [u64; 18],
}

/// Index of `rip` within [`RegisterValues::regs`].
pub const REG_RIP: usize = 16;

// --- Notifications (sys_notif_recv) ----------------------------------------

/// Forward-compat fallback: a discriminant the recipient's ABI doesn't know.
pub const KIND_UNKNOWN: u32 = 0x0000;
/// Hardware exception: page fault / access violation.
pub const KIND_SEG_FAULT: u32 = 0x0100;
/// Hardware exception: invalid opcode.
pub const KIND_ILLEGAL_INSN: u32 = 0x0101;
/// Hardware exception: divide error.
pub const KIND_DIVIDE_BY_ZERO: u32 = 0x0102;
/// Hardware exception: stack overflow.
pub const KIND_STACK_OVERFLOW: u32 = 0x0103;
/// Process lifecycle: a child process exited.
pub const KIND_CHILD_EXITED: u32 = 0x0200;
/// Process lifecycle: an IPC peer closed. **Declared and never emitted** — a dead peer
/// is observed by `sys_channel_recv` answering `PeerClosed` instead, which is what lets
/// a supervisor tell its children's exits apart (`TODO(child-exit-attribution)`).
pub const KIND_PEER_CLOSED: u32 = 0x0201;
/// A holder of this process's handle has asked it to exit (§11h). A request: nothing
/// stops a process that ignores it, because there is no forcible kill.
pub const KIND_TERMINATE_REQUESTED: u32 = 0x0202;
/// Resource: a handle this process holds was invalidated.
pub const KIND_HANDLE_INVALIDATED: u32 = 0x0400;
/// Resource: notifications were dropped due to queue overflow (synthetic).
pub const KIND_NOTIFICATIONS_DROPPED: u32 = 0x0401;

/// `ExitStatus.kind`: voluntary exit; the code is in `code`.
pub const EXIT_NORMAL: u32 = 0;
/// `ExitStatus.kind`: terminated by a supervisor.
pub const EXIT_KILLED: u32 = 1;
/// `ExitStatus.kind`: crashed on a fault; the fault kind is in `code`.
pub const EXIT_CRASHED: u32 = 2;

/// One 64-byte notification record: a `u32` discriminant + a 60-byte LE body.
/// Decode the body by discriminant (`docs/spec/notification-format.md`).
#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct Notification {
    /// Discriminant (`KIND_*`) (offset 0).
    pub kind: u32,
    /// Variant body, little-endian, zero-padded (offset 4).
    pub body: [u8; 60],
}

const _: () = assert!(size_of::<Notification>() == 64);
const _: () = assert!(align_of::<Notification>() == 8);
const _: () = assert!(offset_of!(Notification, kind) == 0);
const _: () = assert!(offset_of!(Notification, body) == 4);

impl Notification {
    /// A zeroed notification (a valid out-param for `sys_notif_recv`).
    pub const fn zeroed() -> Self {
        Self { kind: 0, body: [0u8; 60] }
    }
}

// --- IPC (sys_channel_send / sys_channel_recv) -----------------------------

/// Total size of an [`IpcMsg`], in bytes — one page on x86_64.
pub const IPC_MSG_SIZE: usize = 4096;
/// Size of the [`IpcMsgHeader`] prefix, in bytes.
pub const IPC_HEADER_SIZE: usize = 24;
/// Maximum transferable handles carried by one message.
pub const IPC_HANDLE_MAX: usize = 8;
/// Bytes of inline payload per message.
pub const IPC_PAYLOAD_SIZE: usize = IPC_MSG_SIZE - IPC_HEADER_SIZE - IPC_HANDLE_MAX * 8;

// --- sys_wait --------------------------------------------------------------

/// Maximum handles one `sys_wait` call may block on — the kernel's
/// `MAX_WAIT_HANDLES`. A larger `count` is rejected with `InvalidArgument`.
///
/// This is the **fan-out limit of every resource server**: a server that keeps a channel
/// per client waits on its serving endpoint plus one slot per client, so it serves at most
/// `MAX_WAIT_HANDLES - 1` of them at once. Both servers that do this (`fs-server-ext4`'s
/// directory sessions, `logging-service`'s per-principal sources) derive their cap from
/// this constant rather than restating the number, so raising it is one edit.
///
/// The kernel side is a **fixed per-thread array**, sized rather than allocated so that
/// registering a wait never allocates under the rank-1 scheduler lock. That is what makes
/// this a constant instead of a parameter, and what bounds how far it can sensibly be
/// raised — the cost is paid by every thread, waiting or not.
///
/// Escaping the limit entirely (a readiness mechanism, so one wait slot covers any number
/// of clients) is `TODO(server-fanout)` in `docs/rationale/deferred-decisions.md`.
pub const MAX_WAIT_HANDLES: usize = 32;

/// Bytes one `sys_wait` writes per signaled handle (an `IoResult`).
pub const WAIT_RESULT_SIZE: usize = 24;

/// `SendMode::Block` — block (return a `PendingOperation`) if the ring is full.
pub const SENDMODE_BLOCK: u64 = 0;
/// `SendMode::NoBlock` — fail with `WouldBlock` if the ring is full.
pub const SENDMODE_NOBLOCK: u64 = 1;
/// `SendMode::BlockBounded` — block with a deadline (6th `sys_channel_send` arg).
pub const SENDMODE_BLOCKBOUNDED: u64 = 2;

// --- Service control-channel protocol --------------------------------------
//
// The opcode a supervisor (service-mgr) sends to a service over its per-service
// **control channel** (`[service.<name>.handles].control` in the service schema). A
// slice-A seed of a userspace convention (not a kernel ABI): the opcode is the first
// payload byte of an `IpcMsg`. It will grow (health-check, config reload) and may move
// to a dedicated control-protocol module. See `docs/architecture/service-manager.md`.

/// Control opcode: shut down gracefully and exit. The service should stop its work
/// and call `sys_process_exit(0)`.
pub const CTRL_OP_SHUTDOWN: u8 = 1;

/// The fixed 24-byte IPC message header. `sender_pid`/`timestamp` are stamped by
/// the kernel at send and cannot be forged.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct IpcMsgHeader {
    /// Sending process's PID; set by the kernel (offset 0).
    pub sender_pid: u32,
    /// Valid bytes in `payload[0..payload_len]` (offset 4).
    pub payload_len: u32,
    /// Valid handles in the message's handle array (offset 8).
    pub handle_count: u8,
    /// Padding (offset 9).
    pub _pad1: u8,
    /// `IpcMsgFlags` bitfield (offset 10).
    pub flags: u16,
    /// Padding to 8-byte-align `timestamp` (offset 12).
    pub _pad2: [u8; 4],
    /// Monotonic nanoseconds at enqueue; set by the kernel (offset 16).
    pub timestamp: u64,
}

impl IpcMsgHeader {
    /// An all-zero header.
    pub const ZEROED: IpcMsgHeader = IpcMsgHeader {
        sender_pid: 0,
        payload_len: 0,
        handle_count: 0,
        _pad1: 0,
        flags: 0,
        _pad2: [0; 4],
        timestamp: 0,
    };
}

/// One IPC message: header + inline payload + transferable-handle array; exactly
/// one page, `#[repr(C, align(4096))]`.
#[repr(C, align(4096))]
#[derive(Copy, Clone)]
pub struct IpcMsg {
    /// Fixed 24-byte header (offset 0).
    pub header: IpcMsgHeader,
    /// Inline payload bytes (offset 24).
    pub payload: [u8; IPC_PAYLOAD_SIZE],
    /// Transferable handles (offset 4032).
    pub handles: [u64; IPC_HANDLE_MAX],
}

impl IpcMsg {
    /// An all-zero one-page message (a valid send/recv buffer).
    pub const ZEROED: IpcMsg = IpcMsg {
        header: IpcMsgHeader::ZEROED,
        payload: [0; IPC_PAYLOAD_SIZE],
        handles: [0; IPC_HANDLE_MAX],
    };
}

const _: () = assert!(size_of::<IpcMsgHeader>() == IPC_HEADER_SIZE);
const _: () = assert!(align_of::<IpcMsgHeader>() == 8);
const _: () = assert!(offset_of!(IpcMsgHeader, sender_pid) == 0);
const _: () = assert!(offset_of!(IpcMsgHeader, payload_len) == 4);
const _: () = assert!(offset_of!(IpcMsgHeader, handle_count) == 8);
const _: () = assert!(offset_of!(IpcMsgHeader, flags) == 10);
const _: () = assert!(offset_of!(IpcMsgHeader, timestamp) == 16);
const _: () = assert!(size_of::<IpcMsg>() == IPC_MSG_SIZE);
const _: () = assert!(align_of::<IpcMsg>() == 4096);
const _: () = assert!(offset_of!(IpcMsg, payload) == 24);
const _: () = assert!(offset_of!(IpcMsg, handles) == 4032);

// --- sys_clock_read --------------------------------------------------------

/// `ClockId::Monotonic` — nanoseconds since boot, never decreasing.
pub const CLOCK_MONOTONIC: u64 = 0;

/// `ClockId::Realtime` — nanoseconds since the Unix epoch (UTC).
///
/// Derived as monotonic + a boot-time offset anchored from the hardware RTC, so it
/// advances smoothly and never steps backwards. Returns `Unsupported` on a machine
/// whose RTC could not be read rather than reporting a fabricated epoch.
pub const CLOCK_REALTIME: u64 = 1;

// --- sys_wait completion record --------------------------------------------

/// One completion record `sys_wait` writes per signaled handle; 24 bytes.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoResult {
    /// The signaled handle (offset 0).
    pub handle: u64,
    /// Completion status: `0` = ready/success; negative = `KError` (offset 8).
    pub status: i32,
    /// Reserved; written as `0` (offset 12).
    pub reserved: u32,
    /// Result payload (e.g. a namespace lookup's resolved handle) when
    /// `status == 0`; `0` for edge-style waitables and errors (offset 16).
    pub result: u64,
}

const _: () = assert!(size_of::<IoResult>() == 24);
const _: () = assert!(align_of::<IoResult>() == 8);
const _: () = assert!(offset_of!(IoResult, handle) == 0);
const _: () = assert!(offset_of!(IoResult, status) == 8);
const _: () = assert!(offset_of!(IoResult, reserved) == 12);
const _: () = assert!(offset_of!(IoResult, result) == 16);

// --- sys_io_submit operation descriptor (docs/spec/io-operation.md) ---------

/// `IoOpcode::Read` — device → buffer.
pub const IO_OPCODE_READ: u32 = 0;
/// `IoOpcode::Write` — buffer → device.
pub const IO_OPCODE_WRITE: u32 = 1;

/// The `sys_io_submit` operation descriptor — the userspace mirror of the
/// kernel's `IoOp` (`docs/spec/io-operation.md`). 40 bytes, 8-byte aligned.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IoOp {
    /// Operation selector ([`IO_OPCODE_READ`] / [`IO_OPCODE_WRITE`]) — offset 0.
    pub opcode: u32,
    /// Reserved; must be 0 — offset 4.
    pub flags: u32,
    /// `MemoryObject` handle providing the data buffer — offset 8.
    pub buffer: u64,
    /// Byte offset within `buffer` — offset 16.
    pub buf_offset: u64,
    /// Byte offset within the resource (the device) — offset 24.
    pub offset: u64,
    /// Bytes to transfer — offset 32.
    pub length: u64,
}

const _: () = assert!(size_of::<IoOp>() == 40);
const _: () = assert!(align_of::<IoOp>() == 8);
const _: () = assert!(offset_of!(IoOp, opcode) == 0);
const _: () = assert!(offset_of!(IoOp, flags) == 4);
const _: () = assert!(offset_of!(IoOp, buffer) == 8);
const _: () = assert!(offset_of!(IoOp, buf_offset) == 16);
const _: () = assert!(offset_of!(IoOp, offset) == 24);
const _: () = assert!(offset_of!(IoOp, length) == 32);

// --- sys_handle_stat metadata ----------------------------------------------

/// Handle metadata written by `sys_handle_stat`; 24 bytes.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct HandleInfo {
    /// The handle's current rights bitmask (offset 0).
    pub rights: u64,
    /// The referenced object's `KObjectType` discriminant (`as u32`) (offset 8).
    pub object_type: u32,
    /// The handle's generation counter (offset 12).
    pub generation: u32,
    /// The referenced object's byte size for sized resources (a `MemoryObject`'s
    /// page-rounded size, a `FileObject`'s exact file size), else `0` (offset 16).
    pub size: u64,
}

const _: () = assert!(size_of::<HandleInfo>() == 24);
const _: () = assert!(align_of::<HandleInfo>() == 8);

// --- sys_ns_enumerate ------------------------------------------------------

/// Longest binding path an [`NsEntry`] carries inline; a longer path is truncated
/// (its true length is still reported in `path_len`).
pub const NS_ENTRY_PATH_MAX: usize = 256;

/// [`NsEntry::kind`]: a directly-bound resource handle.
pub const NS_KIND_DIRECT: u32 = 0;
/// [`NsEntry::kind`]: an in-kernel resource server (`/dev/blk`, `/dev/entropy`, …).
pub const NS_KIND_KERNEL: u32 = 1;
/// [`NsEntry::kind`]: a userspace resource server — a **mount** (`/` → fs-server).
pub const NS_KIND_MOUNT: u32 = 2;

/// One namespace binding, written by `sys_ns_enumerate`: its path, target kind
/// (`NS_KIND_*`), and max rights. Lists a namespace's mount points + kernel
/// resources (eshell `mounts`) — not a filesystem `readdir`. `16 + NS_ENTRY_PATH_MAX`
/// bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct NsEntry {
    /// The binding path's true byte length (may exceed `NS_ENTRY_PATH_MAX`).
    pub path_len: u32,
    /// One of `NS_KIND_*`.
    pub kind: u32,
    /// The binding's maximum rights (`Rights::bits()`).
    pub rights: u64,
    /// The binding path bytes (`path[..min(path_len, NS_ENTRY_PATH_MAX)]`).
    pub path: [u8; NS_ENTRY_PATH_MAX],
}

const _: () = assert!(size_of::<NsEntry>() == 16 + NS_ENTRY_PATH_MAX);
const _: () = assert!(align_of::<NsEntry>() == 8);

impl NsEntry {
    /// An all-zero entry (the kernel fills it).
    pub const fn zeroed() -> Self {
        Self { path_len: 0, kind: 0, rights: 0, path: [0; NS_ENTRY_PATH_MAX] }
    }
}
const _: () = assert!(offset_of!(HandleInfo, rights) == 0);
const _: () = assert!(offset_of!(HandleInfo, object_type) == 8);
const _: () = assert!(offset_of!(HandleInfo, generation) == 12);

// --- Framebuffer geometry -----------------------------------------------

/// Geometry and pixel layout of the system framebuffer.
///
/// The userspace mirror of `kernel/src/libkern/framebuffer.rs`. Obtained by mapping
/// `/dev/framebuffer/info`, which resolves to a read-only `MemoryObject` holding exactly
/// one of these; the aperture itself is `/dev/framebuffer`.
///
/// Both sides carry the layout asserts below. `cargo xtask abi-sync-check` deliberately
/// does not compare `#[repr(C)]` layouts — the asserts are the stronger check, and they
/// fail at build time rather than in a separate pass.
///
/// **Channel layout is reported, not assumed.** Firmware does not always choose
/// `0x00RRGGBB`; a client that hardcodes it renders channel-swapped output on hardware
/// that reports BGR.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FramebufferInfo {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row. **Not** `width * bytes_per_pixel` — firmware pads rows.
    pub pitch: u64,
    /// Total mappable bytes of the aperture.
    pub byte_len: u64,
    /// Bits per pixel. Only 32 is served.
    pub bits_per_pixel: u16,
    /// Bit offset of the red channel's least significant bit.
    pub red_shift: u8,
    /// Red channel width in bits.
    pub red_size: u8,
    /// Bit offset of the green channel's least significant bit.
    pub green_shift: u8,
    /// Green channel width in bits.
    pub green_size: u8,
    /// Bit offset of the blue channel's least significant bit.
    pub blue_shift: u8,
    /// Blue channel width in bits.
    pub blue_size: u8,
}

const _: () = assert!(core::mem::size_of::<FramebufferInfo>() == 32);
const _: () = assert!(core::mem::align_of::<FramebufferInfo>() == 8);
const _: () = assert!(offset_of!(FramebufferInfo, width) == 0);
const _: () = assert!(offset_of!(FramebufferInfo, height) == 4);
const _: () = assert!(offset_of!(FramebufferInfo, pitch) == 8);
const _: () = assert!(offset_of!(FramebufferInfo, byte_len) == 16);
const _: () = assert!(offset_of!(FramebufferInfo, bits_per_pixel) == 24);
const _: () = assert!(offset_of!(FramebufferInfo, red_shift) == 26);
const _: () = assert!(offset_of!(FramebufferInfo, red_size) == 27);
const _: () = assert!(offset_of!(FramebufferInfo, green_shift) == 28);
const _: () = assert!(offset_of!(FramebufferInfo, green_size) == 29);
const _: () = assert!(offset_of!(FramebufferInfo, blue_shift) == 30);
const _: () = assert!(offset_of!(FramebufferInfo, blue_size) == 31);

impl FramebufferInfo {
    /// Read one from the first 32 bytes of a mapped `/dev/framebuffer/info` object.
    ///
    /// Returns `None` if the slice is short — a truncated read would otherwise produce
    /// a plausible-looking geometry with zeroed tail fields, and a client would map an
    /// aperture of the wrong size.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| {
            u64::from_le_bytes([
                b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
            ])
        };
        Some(Self {
            width: u32_at(0),
            height: u32_at(4),
            pitch: u64_at(8),
            byte_len: u64_at(16),
            bits_per_pixel: u16::from_le_bytes([b[24], b[25]]),
            red_shift: b[26],
            red_size: b[27],
            green_shift: b[28],
            green_size: b[29],
            blue_shift: b[30],
            blue_size: b[31],
        })
    }
}

// ---------------------------------------------------------------------------
// Input events — the userspace mirror of `kernel/src/libkern/input.rs`.
//
// Records read from a raw input node (`/dev/input/raw/<n>`). The kernel side carries the
// same layout asserts; `cargo xtask abi-sync-check` compares the `EV_*`/`KEY_*`/`REL_*`/
// `BTN_*` constants across the boundary. Before this existed the kernel doc *claimed* a
// mirror that did not, and the only consumer hardcoded the offsets (PR #178 review).
// ---------------------------------------------------------------------------

/// One input event: what happened, on what, when. See the kernel module for the semantics.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InputEvent {
    /// Event class: [`EV_SYN`], [`EV_KEY`], [`EV_REL`], [`EV_ABS`].
    pub kind: u16,
    /// Code within the class — a keycode, [`REL_X`], [`BTN_LEFT`].
    pub code: u16,
    /// Press/release/repeat for `EV_KEY`; a signed delta for `EV_REL`.
    pub value: i32,
    /// Kernel monotonic time at the interrupt.
    pub time_ns: u64,
}

const _: () = assert!(size_of::<InputEvent>() == 16);
const _: () = assert!(align_of::<InputEvent>() == 8);
const _: () = assert!(offset_of!(InputEvent, kind) == 0);
const _: () = assert!(offset_of!(InputEvent, code) == 2);
const _: () = assert!(offset_of!(InputEvent, value) == 4);
const _: () = assert!(offset_of!(InputEvent, time_ns) == 8);

/// Bytes one [`InputEvent`] occupies on the wire.
pub const INPUT_EVENT_LEN: usize = size_of::<InputEvent>();

/// Group separator.
pub const EV_SYN: u16 = 0x00;
/// A key or button changed state.
pub const EV_KEY: u16 = 0x01;
/// A relative axis moved.
pub const EV_REL: u16 = 0x02;
/// An absolute axis was reported.
pub const EV_ABS: u16 = 0x03;

/// End of a logical event group.
pub const SYN_REPORT: u16 = 0;
/// Events were lost; discard accumulated state and resynchronise.
pub const SYN_DROPPED: u16 = 3;

/// The key came up.
pub const KEY_RELEASE: i32 = 0;
/// The key went down.
pub const KEY_PRESS: i32 = 1;
/// Autorepeat while held.
pub const KEY_REPEAT: i32 = 2;

/// Horizontal motion; positive is right.
pub const REL_X: u16 = 0x00;
/// Vertical motion; positive is **down**.
pub const REL_Y: u16 = 0x01;
/// Wheel detents.
pub const REL_WHEEL: u16 = 0x08;


/// Keycodes — `EV_KEY` codes for keyboard keys. Identical to the kernel's table, which
/// takes them from Linux; for `0x01..=0x53` they equal the AT set-1 scancode.
/// Escape. Also the lowest valid keycode: `0` is "no key".
pub const KEY_ESC: u16 = 1;
/// Backspace.
pub const KEY_BACKSPACE: u16 = 14;
/// Tab.
pub const KEY_TAB: u16 = 15;
/// Return/Enter on the main block.
pub const KEY_ENTER: u16 = 28;
/// Left Control.
pub const KEY_LEFTCTRL: u16 = 29;
/// Left Shift.
pub const KEY_LEFTSHIFT: u16 = 42;
/// Right Shift.
pub const KEY_RIGHTSHIFT: u16 = 54;
/// Left Alt.
pub const KEY_LEFTALT: u16 = 56;
/// Space.
pub const KEY_SPACE: u16 = 57;
/// Caps Lock.
pub const KEY_CAPSLOCK: u16 = 58;
/// F1.
pub const KEY_F1: u16 = 59;
/// Num Lock.
pub const KEY_NUMLOCK: u16 = 69;
/// Scroll Lock.
pub const KEY_SCROLLLOCK: u16 = 70;
/// Keypad `.`/Del — the highest key reachable without an `E0` prefix.
pub const KEY_KPDOT: u16 = 83;
/// Keypad Enter (`E0 1C`).
pub const KEY_KPENTER: u16 = 96;
/// Right Control (`E0 1D`).
pub const KEY_RIGHTCTRL: u16 = 97;
/// Keypad `/` (`E0 35`).
pub const KEY_KPSLASH: u16 = 98;
/// Right Alt / AltGr (`E0 38`).
pub const KEY_RIGHTALT: u16 = 100;
/// Home (`E0 47`).
pub const KEY_HOME: u16 = 102;
/// Cursor up (`E0 48`).
pub const KEY_UP: u16 = 103;
/// Page Up (`E0 49`).
pub const KEY_PAGEUP: u16 = 104;
/// Cursor left (`E0 4B`).
pub const KEY_LEFT: u16 = 105;
/// Cursor right (`E0 4D`).
pub const KEY_RIGHT: u16 = 106;
/// End (`E0 4F`).
pub const KEY_END: u16 = 107;
/// Cursor down (`E0 50`).
pub const KEY_DOWN: u16 = 108;
/// Page Down (`E0 51`).
pub const KEY_PAGEDOWN: u16 = 109;
/// Insert (`E0 52`).
pub const KEY_INSERT: u16 = 110;
/// Delete (`E0 53`).
pub const KEY_DELETE: u16 = 111;
/// (`display-substrate.md` §5a).
pub const KEY_LEFTMETA: u16 = 125;
/// Right Meta (`E0 5C`).
pub const KEY_RIGHTMETA: u16 = 126;
/// Menu / Compose (`E0 5D`).
pub const KEY_COMPOSE: u16 = 127;

/// Primary button.
pub const BTN_LEFT: u16 = 0x110;
/// Secondary button.
pub const BTN_RIGHT: u16 = 0x111;
/// Wheel click.
pub const BTN_MIDDLE: u16 = 0x112;

impl InputEvent {
    /// Parse from exactly [`INPUT_EVENT_LEN`] little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < INPUT_EVENT_LEN {
            return None;
        }
        Some(Self {
            kind: u16::from_le_bytes([b[0], b[1]]),
            code: u16::from_le_bytes([b[2], b[3]]),
            value: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            time_ns: u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]),
        })
    }
}
