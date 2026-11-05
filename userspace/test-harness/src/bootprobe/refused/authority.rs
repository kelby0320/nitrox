//! Calls that pass what no libos type names: a handle that is not one, or of the wrong kind, a
//! mapping that asks for no right, a handle listed twice, a lookup's own `PendingOperation`, and
//! the monotonic clock.

use libkern::{
    CLOCK_MONOTONIC, POWER_HALT, Rights, SENDMODE_NOBLOCK, SYS_CHANNEL_SEND, SYS_CLOCK_SET, SYS_HANDLE_CLOSE,
    SYS_HANDLE_DUPLICATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_POWER, SYS_WAIT, syscall1, syscall2,
    syscall4, syscall5,
};
use libos::{Channel, Handle, Namespace, NsReadOnly, Only, Ref};

/// What `sys_power` answers for the raw handle `h`, which the machine halts on if it is taken.
pub(crate) fn power(h: u64) -> i64 {
    // SAFETY: register-only syscall; refused before anything happens, which is the point.
    unsafe { syscall2(SYS_POWER, h, POWER_HALT) }
}

/// What `sys_clock_set` answers for `ns` on the monotonic clock, which libos's `set_clock` never
/// names.
pub(crate) fn set_monotonic_clock(ns: u64) -> i64 {
    // SAFETY: register-only syscall.
    unsafe { syscall2(SYS_CLOCK_SET, CLOCK_MONOTONIC, ns) }
}

/// What mapping `len` bytes of `h` asking for `rights` answers: the address, unmapped at once, or
/// the refusal. libos's `map` always asks for the principal rights of its type.
pub(crate) fn map_asking<T, M>(h: &Handle<T, M>, len: u64, rights: u64) -> i64 {
    // SAFETY: register-only syscall on a handle this process holds.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h.raw().0, 0, len, rights) };
    if addr > 0 {
        // SAFETY: unmapping the mapping just made, which nothing read or wrote.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, len) };
    }
    addr
}

/// What sending an empty message on `ch` with the raw handles `listed` answers; libos moves each
/// handle once, so it cannot list one twice.
pub(crate) fn send_listing(ch: &Handle<Channel, Only>, listed: &[u64]) -> i64 {
    let msg = [0u8; 4096];
    // SAFETY: a valid message buffer and handle array.
    unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            ch.raw().0,
            msg.as_ptr() as u64,
            listed.as_ptr() as u64,
            listed.len() as u64,
            SENDMODE_NOBLOCK,
        )
    }
}

/// What became of a lookup's `PendingOperation` handle.
pub(crate) struct Stays {
    /// What duplicating it answered.
    pub duplicate: i64,
    /// What sending it on a channel answered.
    pub send: i64,
    /// Whether the lookup, waited on after both, resolved.
    pub resolved: bool,
}

/// Start a lookup of `path` and try to duplicate and send its `PendingOperation`, then wait it
/// out: libos keeps the PO inside the `Op`, where nothing can name it.
pub(crate) fn lookup_po_stays(ns: Ref<'_, Namespace, NsReadOnly>, path: &str) -> Option<Stays> {
    // SAFETY: a valid path and a namespace handle this process holds.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns.raw().0, path.as_ptr() as u64, path.len() as u64, Rights::MAP_READ.bits())
    };
    let channel = Handle::<Channel, Only>::pair(4);
    let (po, (a, b)) = match (po > 0, channel) {
        (true, Ok(pair)) => (po as u64, pair),
        _ => return None,
    };
    // SAFETY: register-only syscall on a handle this process holds.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, po, (Rights::WAIT | Rights::INSPECT).bits()) };
    let send = send_listing(&a, &[po]);
    drop((a, b));
    if dup > 0 {
        // SAFETY: closing a handle this process holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, dup as u64) };
    }
    // Read last, its handle closed: a PO the send took would be gone, its wait failing.
    let handles = [po];
    let mut result = [0u8; 24];
    // SAFETY: one handle this process holds, and room for one result.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, result.as_mut_ptr() as u64, u64::MAX) };
    let status = i32::from_le_bytes([result[8], result[9], result[10], result[11]]);
    let image = u64::from_le_bytes([
        result[16], result[17], result[18], result[19], result[20], result[21], result[22], result[23],
    ]);
    // SAFETY: closing handles this process holds: the PO, and the file it resolved to.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, po);
        if waited == 1 && status == 0 && image != 0 {
            syscall1(SYS_HANDLE_CLOSE, image);
        }
    }
    Some(Stays { duplicate: dup, send, resolved: waited == 1 && status == 0 && image != 0 })
}
