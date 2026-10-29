//! Kernel-internal data structures and primitives.
//!
//! The kernel's own `libkern`: in-kernel synchronisation primitives, heap-backed containers,
//! and small utilities the rest of the kernel builds on. Distinct from `userspace/libkern/`,
//! the raw syscall layer for user-mode code. See `docs/architecture/memory-management.md` for
//! how this module fits with the buddy and slab allocators.
//!
//! ## Fallible allocation
//!
//! The kernel registers no `#[global_allocator]` and does not use the `alloc` crate: every
//! type in `alloc` aborts on allocation failure, which a kernel cannot tolerate. [`KBox`],
//! [`KVec`], and [`KString`] call the slab allocator directly and report exhaustion as
//! [`AllocError`]. See the decision log entry of 2026-05-20.

pub mod arrayvec;
pub mod block;
pub mod chacha;
pub mod clock;
pub mod device;
pub mod framebuffer;
pub mod handle;
pub mod input;
pub mod io_op;
pub mod io_result;
pub mod ipc;
pub mod kbox;
pub mod kstring;
pub mod kvec;
pub mod lockrank;
pub mod memory;
pub mod notification;
pub mod pod;
pub mod power;
pub mod printable;
pub mod reserved;
pub mod spawn;
pub mod spinlock;
pub mod syscaps;
pub mod thread;
pub mod timer_flags;

pub use arrayvec::ArrayVec;
pub use chacha::ChaCha20Rng;
pub use clock::ClockId;
pub use handle::{KObjectType, RawHandle, Rights};
pub use io_op::{IoOp, IoOpcode};
pub use io_result::IoResult;
pub use ipc::{IpcMsg, IpcMsgHeader, SendMode};
pub use kbox::KBox;
pub use kstring::KString;
pub use kvec::KVec;
pub use memory::MemFlags;
pub use notification::{ExitKind, ExitStatus, FaultKind, Notification};
pub use pod::Pod;
pub use reserved::Reserved;
pub use spawn::{SPAWN_MAX_HANDLES, SpawnArgs};
pub use spinlock::{IrqSpinLock, IrqSpinLockGuard, SpinLock, SpinLockGuard};
pub use syscaps::SysCaps;
pub use thread::ThreadArgs;
pub use timer_flags::TimerFlags;

/// Error returned by the fallible `libkern` allocators ([`KBox`], [`KVec`], [`KString`])
/// when the kernel heap cannot satisfy a request.
///
/// Allocation failure is a recoverable condition that must propagate as a `Result`, never a
/// panic; see the decision log entry of 2026-05-20.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocError;
