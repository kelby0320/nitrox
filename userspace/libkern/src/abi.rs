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
/// `ImageId::Child` — the Phase-1 IPC demo child (kernel-embedded image selector).
pub const IMAGE_CHILD: u32 = 0;
/// `ImageId::Init` — the bootstrapping init (`userspace/init`), kernel-embedded.
pub const IMAGE_INIT: u32 = 1;
/// `ImageId::Parent` — the demo supervisor (`userspace/parent`), kernel-embedded.
pub const IMAGE_PARENT: u32 = 2;
/// `ImageId::FsServerExt4` — the ext4 filesystem server (`userspace/fs-server-ext4`),
/// kernel-embedded; spawned by init (slice 7).
pub const IMAGE_FS_SERVER_EXT4: u32 = 3;
/// `ImageId::Eshell` — the emergency shell (`userspace/eshell`), kernel-embedded;
/// spawned by init (slice 9).
pub const IMAGE_ESHELL: u32 = 4;

/// The spawn argument block, passed by pointer to `sys_process_spawn`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SpawnArgs {
    /// Executable image selector (`ImageId`) (offset 0).
    pub image: u32,
    /// Valid entries in `handles`/`rights`; `≤ SPAWN_MAX_HANDLES` (offset 4).
    pub handle_count: u32,
    /// Bit `i` set ⇒ **move** `handles[i]` to the child; clear ⇒ duplicate (offset 8).
    pub move_mask: u32,
    /// Padding to 8-byte-align `arg0` (offset 12).
    pub _pad: u32,
    /// Opaque user data handed to the child at entry (in `rcx`) (offset 16).
    pub arg0: u64,
    /// Parent-side handles to install in the child's table (offset 24).
    pub handles: [u64; SPAWN_MAX_HANDLES],
    /// Per-handle rights attenuation bound; installed = `source & rights[i]` (offset 56).
    pub rights: [u64; SPAWN_MAX_HANDLES],
    /// Child's root namespace; `0` ⇒ inherit a LOOKUP-only handle to the parent's
    /// namespace, non-null ⇒ a (restricted) namespace the parent holds (offset 88).
    pub namespace: u64,
}

const _: () = assert!(size_of::<SpawnArgs>() == 96);
const _: () = assert!(align_of::<SpawnArgs>() == 8);
const _: () = assert!(offset_of!(SpawnArgs, image) == 0);
const _: () = assert!(offset_of!(SpawnArgs, handle_count) == 4);
const _: () = assert!(offset_of!(SpawnArgs, move_mask) == 8);
const _: () = assert!(offset_of!(SpawnArgs, arg0) == 16);
const _: () = assert!(offset_of!(SpawnArgs, handles) == 24);
const _: () = assert!(offset_of!(SpawnArgs, rights) == 56);
const _: () = assert!(offset_of!(SpawnArgs, namespace) == 88);

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
    /// Reserved; must be zero (offset 24).
    pub _reserved: [u8; 40],
}

const _: () = assert!(size_of::<ThreadArgs>() == 64);
const _: () = assert!(align_of::<ThreadArgs>() == 8);
const _: () = assert!(offset_of!(ThreadArgs, entry) == 0);
const _: () = assert!(offset_of!(ThreadArgs, user_sp) == 8);
const _: () = assert!(offset_of!(ThreadArgs, arg0) == 16);

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
/// Process lifecycle: an IPC peer closed.
pub const KIND_PEER_CLOSED: u32 = 0x0201;
/// Resource: a handle was invalidated.
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

/// `SendMode::Block` — block (return a `PendingOperation`) if the ring is full.
pub const SENDMODE_BLOCK: u64 = 0;
/// `SendMode::NoBlock` — fail with `WouldBlock` if the ring is full.
pub const SENDMODE_NOBLOCK: u64 = 1;
/// `SendMode::BlockBounded` — block with a deadline (6th `sys_channel_send` arg).
pub const SENDMODE_BLOCKBOUNDED: u64 = 2;

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
