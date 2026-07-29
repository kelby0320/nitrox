//! The serial **console input** driver (Tier 1) — COM1 receive.
//!
//! The kernel owns the UART (port I/O is kernel-only), so getting keyboard input
//! from the serial line to userspace is an in-kernel driver. It is reached through
//! the **universal device interface**: the console is a char-class
//! [`DeviceNode`] bound at `/dev/console`, read with `sys_io_submit(Read)` like any
//! device. There is no console-specific syscall.
//!
//! ## Flow
//!
//! ```text
//! COM1 RX IRQ4 → console_isr (drain UART → ring) → enqueue CONSOLE_DPC
//!   submit_read: ring has bytes → copy + complete PO now (pre-signalled)
//!                ring empty      → park the read; CONSOLE_DPC completes it later
//! ```
//!
//! The read's [`PendingOperation`](crate::object::PendingOperation) is the wait
//! target (mirroring block I/O's IRP completion) — no internal `InterruptObject` is
//! needed. Echo and line editing live in userspace (eshell); the kernel delivers
//! raw bytes.
//!
//! ## Locking
//!
//! All state lives behind one [`IrqSpinLock`] (masks interrupts while held), so the
//! ISR cannot preempt a syscall-context ring operation. The lock is released before
//! touching the buffer `MemoryObject` or `complete_pending_op` (which takes the
//! rank-1 `SCHED` lock) — never nested. Single-reader (eshell) in Phase 2.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::dpc::Dpc;
use crate::libkern::KBox;
use crate::libkern::IrqSpinLock;
use crate::libkern::handle::KObjectType;
use crate::mm::{PAGE_SIZE, heap};
use crate::object::device_node::{CharBackend, ResourceDescriptor};
use crate::object::{DeviceNode, MemoryObject, ObjectRef};
use crate::syscall::error::KError;

/// Capacity of the RX ring — ample for an interactive command line; excess input
/// (rare) is dropped rather than blocking the ISR.
const RING_CAP: usize = 256;

/// A parked `sys_io_submit(Read)` awaiting input: the references it pins and where
/// to land the bytes. Completed (and dropped) by [`console_intr_dpc`].
struct ParkedRead {
    po: ObjectRef,
    buffer: ObjectRef,
    buf_offset: u64,
    max_len: usize,
}

/// All console-input state, behind [`CONSOLE`].
struct Inner {
    /// Circular RX byte ring (`head` = next byte to read, `len` = bytes present).
    ring: [u8; RING_CAP],
    head: usize,
    len: usize,
    /// The single outstanding parked read, or `None`.
    parked: Option<ParkedRead>,
}

impl Inner {
    const fn new() -> Self {
        Inner { ring: [0; RING_CAP], head: 0, len: 0, parked: None }
    }

    /// Push one received byte; drops it if the ring is full.
    fn push(&mut self, b: u8) {
        if self.len < RING_CAP {
            let tail = (self.head + self.len) % RING_CAP;
            self.ring[tail] = b;
            self.len += 1;
        }
    }

    /// Pop up to `dst.len()` buffered bytes into `dst`; returns the count.
    fn pop_into(&mut self, dst: &mut [u8]) -> usize {
        let n = self.len.min(dst.len());
        for d in dst.iter_mut().take(n) {
            *d = self.ring[self.head];
            self.head = (self.head + 1) % RING_CAP;
        }
        self.len -= n;
        n
    }
}

static CONSOLE: IrqSpinLock<Inner> = IrqSpinLock::new(Inner::new());

/// Completes the parked read after the ISR deposits bytes (queued by the ISR).
static CONSOLE_DPC: Dpc = Dpc::new(console_intr_dpc, core::ptr::null_mut());

/// The leaked-`'static` console [`DeviceNode`] (refcount never drops to zero);
/// [`device_ref`] hands out counted references for `/dev/console` lookups.
static CONSOLE_NODE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Copy `src` into `buffer`'s frames starting at byte `buf_offset`, via the HHDM.
/// The caller has bounds-checked `buf_offset + src.len() <= buffer.size()`
/// (`sys_io_submit` does). Runs outside the console lock; the `MemoryObject` frames
/// are kernel memory (not user memory), so this is sound from a DPC too.
fn copy_into_memobj(buffer: &ObjectRef, buf_offset: u64, src: &[u8]) {
    // SAFETY: `buffer` pins a live `MemoryObject`.
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    let frames = mo.frames();
    let hhdm = heap::hhdm_offset();
    let mut pos = buf_offset as usize;
    for &b in src {
        let page = pos / PAGE_SIZE;
        let intra = pos % PAGE_SIZE;
        if page >= frames.len() {
            break;
        }
        let dst = (frames[page].as_u64() + hhdm) as *mut u8;
        // SAFETY: `dst.add(intra)` is within an owned, HHDM-mapped buffer frame
        // (bounds pre-checked by the caller).
        unsafe { *dst.add(intra) = b };
        pos += 1;
    }
}

/// [`CharBackend::submit_read`] for the console: satisfy the read immediately from
/// the RX ring if any bytes are buffered (pre-signalling the PO), else park it for
/// the next RX interrupt. Returns `WouldBlock` if a read is already pending (the
/// Phase-2 console is single-reader). Runs in syscall context.
fn submit_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    max_len: u64,
    _ctx: *mut (),
) -> Result<(), KError> {
    let max_len = (max_len as usize).min(RING_CAP);
    let mut tmp = [0u8; RING_CAP];
    // Decide under the lock; do the copy + PO completion after releasing it.
    let drained = {
        let mut g = CONSOLE.lock();
        if g.len > 0 {
            Some(g.pop_into(&mut tmp[..max_len]))
        } else if g.parked.is_some() {
            return Err(KError::WouldBlock); // single reader
        } else {
            g.parked = Some(ParkedRead {
                po: po.clone(),
                buffer: buffer.clone(),
                buf_offset,
                max_len,
            });
            None
        }
    };
    if let Some(n) = drained {
        copy_into_memobj(buffer, buf_offset, &tmp[..n]);
        crate::sched::complete_pending_op(po.as_ptr(), 0, n as u64);
    }
    Ok(())
}

/// DPC: if a read is parked and the ring has bytes, drain them into the parked
/// buffer and complete its PO (waking the reader). Queued by [`console_isr`].
fn console_intr_dpc(_ctx: *mut ()) {
    let mut tmp = [0u8; RING_CAP];
    let completed = {
        let mut g = CONSOLE.lock();
        if g.parked.is_some() && g.len > 0 {
            let pr = g.parked.take().expect("checked is_some");
            let take = pr.max_len.min(RING_CAP);
            let n = g.pop_into(&mut tmp[..take]);
            Some((pr.po, pr.buffer, pr.buf_offset, n))
        } else {
            None
        }
    };
    if let Some((po, buffer, buf_offset, n)) = completed {
        copy_into_memobj(&buffer, buf_offset, &tmp[..n]);
        crate::sched::complete_pending_op(po.as_ptr(), 0, n as u64);
        // `po` / `buffer` drop here (outside the lock) — refcount decrements only
        // (the caller's handles keep the objects alive).
    }
}

/// COM1 RX interrupt handler: drain every available byte into the ring, then — if a
/// read is parked — queue the completion DPC. Minimal (no `SCHED`, no copy); the
/// dispatch epilogue EOIs the local APIC after this returns. Reading `REG_DATA`
/// clears the UART's RX-data-available condition.
extern "C" fn console_isr() {
    let parked = {
        let mut g = CONSOLE.lock();
        while crate::arch::serial::console_rx_ready() {
            let b = crate::arch::serial::console_rx_read();
            g.push(b);
        }
        g.parked.is_some()
    };
    if parked {
        crate::dpc::enqueue(&CONSOLE_DPC);
    }
}

/// A counted reference to the console [`DeviceNode`] (for the `/dev/console`
/// kernel server), or `None` before [`init`] runs.
pub fn device_ref() -> Option<ObjectRef> {
    let p = CONSOLE_NODE.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: `CONSOLE_NODE` points at the leaked-`'static` console `DeviceNode`
    // (its creation reference is never released), so the object is live.
    unsafe { ObjectRef::try_acquire(p, KObjectType::DeviceNode) }
}

/// Bring up console input: self-test the RX path via internal loopback, publish the
/// console `DeviceNode`, then arm COM1's RX interrupt (IRQ 4). Call once at boot,
/// after the interrupt router is initialised and with interrupts masked (the
/// loopback self-test polls, so it must run before RX IRQs are armed). Logs a
/// one-line result; not a `panic!` path.
pub fn init() {
    // 1. Prove the RX register path deterministically (interrupts still masked).
    if crate::arch::serial::console_rx_loopback_selftest() {
        crate::kprintln!("console: RX loopback self-test OK");
    } else {
        crate::kprintln!("console: RX loopback self-test FAIL");
    }

    // 2. Publish the console as a char DeviceNode (leaked `'static`).
    let backend = CharBackend { submit_read, ctx: core::ptr::null_mut() };
    match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
        Ok(node) => {
            // Leak the creation reference: the console lives for the kernel's
            // lifetime. `device_ref` hands out counted references off this pointer.
            let ptr = KBox::into_raw(node).as_ptr() as *mut ();
            CONSOLE_NODE.store(ptr, Ordering::Release);
        }
        Err(_) => {
            crate::kprintln!("console: device-node alloc FAIL (no /dev/console)");
            return;
        }
    }

    // 3. Arm RX: route the console UART's interrupt to our ISR and enable RX. The
    // platform wiring (which IRQ, how routed) stays inside the arch console module.
    // SAFETY: ring-0, after `IrqRouter::init`; `console_isr` is valid for the
    // kernel's lifetime and we are ready to receive the IRQ.
    let vector = unsafe { crate::arch::serial::console_arm_rx(console_isr) };
    crate::kprintln!("console: RX armed (vec{:#x})", vector);
}
