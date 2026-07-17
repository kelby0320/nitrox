//! Kernel object substrate.
//!
//! A *kernel object* is anything a handle can refer to: it lives in
//! kernel memory, is reference-counted with a kernel-managed lifetime,
//! and has a type with associated operations (see
//! `docs/architecture/overview.md` § "Kernel objects"). The handle table
//! in [`crate::handle`] is the capability lookup layer that maps handles
//! to the type-erased pointers this module's objects live behind; the
//! two are deliberately separate modules (decision log, 2026-05-28).
//!
//! Every object begins with a [`KObjectHeader`] (refcount + type tag) as
//! its first `#[repr(C)]` field, and type-specific operations dispatch
//! through a `match` on [`KObjectType`](crate::libkern::handle::KObjectType)
//! rather than `dyn` (per `kernel/CLAUDE.md`). [`ObjectRef`] is the RAII
//! refcount holder that `HandleTable::lookup` hands back; dropping it
//! releases the reference and, on the last one, runs the concrete
//! destructor.
//!
//! This slice implements the substrate plus the first two concrete
//! objects, [`Process`] and [`Thread`]. The remaining object types land
//! behind their respective Phase 1 slices.

pub mod device_node;
pub mod entropy_object;
pub mod file_object;
pub mod header;
pub mod interrupt_object;
pub mod ipc_channel;
pub mod kernel_server;
pub mod memory_object;
pub mod namespace;
pub mod notification_channel;
pub mod pending_op;
pub mod process;
pub mod thread;
pub mod timer;
pub mod userspace_server;

pub use device_node::DeviceNode;
pub use entropy_object::EntropyObject;
pub use file_object::{BlockRun, FileObject, PageState, Producer, Reserve};
pub use header::{KObjectHeader, ObjectRef};
pub use interrupt_object::InterruptObject;
pub use ipc_channel::{
    BlockSendOutcome, IpcChannel, ReclaimedSend, RecvState, SendOutcome, StoredMsg, TransferRef,
};
pub use kernel_server::{KernelServerId, OpStatus};
pub use memory_object::MemoryObject;
pub use namespace::{BindingTarget, Namespace, NsError, ResolvedTarget};
pub use notification_channel::NotificationChannel;
pub use pending_op::PendingOperation;
pub use process::Process;
pub use thread::{MAX_WAIT_HANDLES, SchedClass, Thread, ThreadEntry, ThreadState, WaitPhase};
pub use timer::Timer;
pub use userspace_server::UserspaceServerReg;
