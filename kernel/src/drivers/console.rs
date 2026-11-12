//! The serial **console input** driver (Tier 1) — COM1 receive.
//!
//! The kernel owns the UART, so serial input reaches userspace as a char-class [`DeviceNode`] at
//! `/dev/console`, read with `sys_io_submit(Read)`; echo and line editing live in userspace.
//!
//! ```text
//! COM1 RX IRQ4 → console_isr (drain UART → ring) → enqueue CONSOLE_DPC
//!   submit_read: ring has bytes → copy + complete PO now (pre-signalled)
//!                ring empty      → park the read; CONSOLE_DPC completes it later
//! ```
//!
//! All state lives behind one [`IrqSpinLock`], so the ISR cannot preempt a ring operation; the lock
//! is released before touching the buffer `MemoryObject` or `complete_pending_op` (rank-1 `SCHED`),
//! never nested. One reader at a time.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::dpc::Dpc;
use crate::drivers::input::{self, Drain, ParkedRead, Reader, Submitted};
use crate::libkern::IrqSpinLock;
use crate::libkern::KBox;
use crate::libkern::lockrank::LockRank;
use crate::object::device_node::{CharBackend, ResourceDescriptor};
use crate::object::header::Ref;
use crate::object::{DeviceNode, MemoryObject, PendingOperation};
use crate::syscall::error::KError;

/// Capacity of the RX ring; input beyond it is dropped rather than blocking the ISR.
const RING_CAP: usize = 256;

/// The received bytes, read as they came.
struct Bytes {
    ring: [u8; RING_CAP],
    head: usize,
    len: usize,
}

impl Bytes {
    const fn new() -> Self {
        Bytes { ring: [0; RING_CAP], head: 0, len: 0 }
    }

    fn push(&mut self, b: u8) {
        if self.len < RING_CAP {
            let tail = (self.head + self.len) % RING_CAP;
            self.ring[tail] = b;
            self.len += 1;
        }
    }

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

impl Drain for Bytes {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn drain_into(&mut self, out: &mut [u8], _now_ns: u64) -> usize {
        self.pop_into(out)
    }
}

static CONSOLE: IrqSpinLock<Reader<Bytes>> = IrqSpinLock::new(LockRank::Leaf, Reader::over(Bytes::new()));

static CONSOLE_DPC: Dpc = Dpc::new(console_intr_dpc, core::ptr::null_mut());

static CONSOLE_NODE: AtomicPtr<DeviceNode> = AtomicPtr::new(core::ptr::null_mut());

/// [`CharBackend::submit_read`] for the console: satisfy the read now from the ring or park it for
/// the next RX interrupt; `WouldBlock` if one is already pending. Syscall context.
fn submit_read(
    buffer: &Ref<MemoryObject>,
    po: &Ref<PendingOperation>,
    buf_offset: u64,
    _offset: u64,
    max_len: u64,
    _ctx: *mut (),
) -> Result<(), KError> {
    read(&CONSOLE, buffer, po, buf_offset, (max_len as usize).min(RING_CAP))
}

fn read(
    console: &IrqSpinLock<Reader<Bytes>>,
    buffer: &Ref<MemoryObject>,
    po: &Ref<PendingOperation>,
    buf_offset: u64,
    max_len: usize,
) -> Result<(), KError> {
    let mut tmp = [0u8; RING_CAP];
    let found = input::read_or_park(
        || reclaim(console),
        || console.lock().read_now(&mut tmp[..max_len], 0, || ParkedRead::new(po, buffer, buf_offset, max_len)),
    );
    match found {
        Submitted::Drained(n) => {
            input::deliver_now(buffer, po, buf_offset, &tmp[..n]);
            Ok(())
        }
        Submitted::Parked => Ok(()),
        Submitted::Busy => Err(KError::WouldBlock), // single reader
        Submitted::Gone => Err(KError::PeerClosed),
    }
}

/// DPC: if a read is parked and the ring has bytes, drain them into it and complete its PO.
fn console_intr_dpc(_ctx: *mut ()) {
    let mut tmp = [0u8; RING_CAP];
    // Given back, not dropped: a process that exits leaves a read's last references here.
    let ready = CONSOLE.lock().take_ready(&mut tmp, 0);
    if let Some((read, n)) = ready {
        input::deliver(&read, &tmp[..n]);
        CONSOLE.lock().owe(read);
    }
}

/// Drop a `ParkedRead` the DPC has finished with, from `sched::reap_pending` and before a new read
/// parks: **thread context only**, as the refs it pinned are released here.
fn reclaim_completed() {
    reclaim(&CONSOLE);
}

fn reclaim(console: &IrqSpinLock<Reader<Bytes>>) {
    // Take under the lock, drop outside it: a drop reaches the allocator.
    let owed = console.lock().take_owed();
    drop(owed);
}

/// COM1 RX interrupt handler: drain every available byte into the ring, then queue the completion
/// DPC if a read is parked. Minimal — no `SCHED`, no copy.
extern "C" fn console_isr() {
    let parked = {
        let mut g = CONSOLE.lock();
        while crate::arch::serial::console_rx_ready() {
            let b = crate::arch::serial::console_rx_read();
            g.ring.push(b);
        }
        g.has_parked()
    };
    if parked {
        crate::dpc::enqueue(&CONSOLE_DPC);
    }
}

/// A counted reference to the console [`DeviceNode`], or `None` before [`init`] runs.
pub fn device_ref() -> Option<Ref<DeviceNode>> {
    let p = CONSOLE_NODE.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: `CONSOLE_NODE` points at the leaked-`'static` console `DeviceNode`
    // (its creation reference is never released), so the object is live.
    unsafe { Ref::try_acquire(p) }
}

/// Bring up console input: self-test RX by internal loopback, publish the console `DeviceNode`, arm
/// COM1's RX interrupt (IRQ 4). Call once at boot with interrupts masked (the self-test polls).
pub fn init() {
    // 1. Prove the RX path once a UART is known to be there; the laptop has none, and without one
    // the self-test fails as on a broken UART.
    if !crate::arch::serial::console_present() {
        crate::kprintln!("console: no UART at COM1");
    } else if crate::arch::serial::console_rx_loopback_selftest() {
        crate::kprintln!("console: RX loopback self-test OK");
    } else {
        crate::kprintln!("console: RX loopback self-test FAIL");
    }

    let backend = CharBackend { submit_read, submit_write: None, ctx: core::ptr::null_mut() };
    match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
        Ok(node) => {
            if !crate::sched::register_reclaim(reclaim_completed) {
                crate::kprintln!("console: its reads are reclaimed only as the next parks");
            }
            CONSOLE_NODE.store(KBox::into_raw(node).as_ptr(), Ordering::Release);
            // **And in the device table**, so `/dev/registry` lists it; it is served at
            // `/dev/console`, which no index names.
            if let Some(r) = device_ref() {
                crate::device::register_char(
                    r,
                    crate::libkern::device::DeviceKind::Console,
                    crate::libkern::device::NOT_SERVED,
                    "console",
                );
            }
        }
        Err(_) => {
            crate::kprintln!("console: device-node alloc FAIL (no /dev/console)");
            return;
        }
    }

    // 3. Arm RX; which IRQ and how it is routed stays inside the arch console module.
    // SAFETY: ring-0, after `IrqRouter::init`; `console_isr` is valid for the
    // kernel's lifetime and we are ready to receive the IRQ.
    match unsafe { crate::arch::serial::console_arm_rx(console_isr) } {
        Ok(vector) => crate::kprintln!("console: RX armed (vec{:#x})", vector),
        Err(e) => crate::kprintln!("console: RX not armed — {}", e.says()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::lockrank::LockRank;

    #[test]
    fn the_rings_bytes_are_read_in_order_and_past_its_capacity_dropped() {
        let mut ring = Bytes::new();
        for b in 0..RING_CAP + 4 {
            ring.push(b as u8);
        }
        let mut out = [0u8; RING_CAP + 4];
        assert_eq!(ring.drain_into(&mut out[..3], 0), 3);
        assert_eq!(&out[..3], &[0, 1, 2]);
        assert_eq!(ring.drain_into(&mut out, 0), RING_CAP - 3, "what fit");
        assert!(ring.is_empty());
    }

    #[test]
    fn a_console_read_through_the_driver_waits_out_the_delivery_before_it() {
        input::test_support::a_read_waits_out_the_delivery_before_it(
            IrqSpinLock::new(LockRank::Leaf, Reader::over(Bytes::new())),
            |console, buf, po| super::read(console, buf, po, 0, 1),
            |console| {
                let mut g = console.lock();
                g.ring.push(b'x');
                g.take_ready(&mut [0u8; RING_CAP], 0).unwrap().0
            },
            |console, first| console.lock().owe(first),
            |console| console.lock().has_parked(),
        );
    }
}
