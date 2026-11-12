//! The default endpoint's control transfers: a request, its data page, and a halt recovered.

use super::*;

/// **Recover a halted default endpoint** (xHCI 1.2 §4.6.8, §4.6.10): Reset Endpoint takes it from
/// Halted to Stopped, and Set TR Dequeue Pointer moves it to the ring's enqueue point — past what is
/// left of the stalled transfer — with the ring's cycle state. The next doorbell starts it there.
pub(in crate::drivers::xhci) fn recover_ep0(x: &Xhci, slot: u8, mem: &DeviceMem) -> Result<(), Failed> {
    command(x, Trb::endpoint_command(kind::RESET_ENDPOINT, slot, DCI_EP0))?;
    let at = mem.ep0.phys().as_u64() + mem.ring.next_slot() as u64 * 16;
    command(x, Trb::set_dequeue(at, mem.ring.cycle(), slot, DCI_EP0))?;
    Ok(())
}

/// **A GET_DESCRIPTOR** of `kind`, index `index`, in language `lang`, up to `len` bytes, on the
/// default endpoint. The bytes, as many as asked: a device that sent fewer left the rest zero.
#[allow(clippy::too_many_arguments, reason = "the request's fields, and where its answer goes")]
pub(super) fn read_descriptor<'o>(
    x: &Xhci,
    slot: u8,
    mem: &mut DeviceMem,
    kind: u8,
    index: u8,
    lang: u16,
    len: u16,
    out: &'o mut [u8],
) -> Result<&'o [u8], Failed> {
    control_in(x, slot, mem, get_descriptor(kind, index, lang, len), len)?;
    Ok(data_bytes(mem, len, out))
}

/// **A GET_DESCRIPTOR to interface `iface`**: a HID report descriptor, up to `out`'s length and a
/// page. **The bytes that arrived**, fewer than asked for when the device sent less.
pub(in crate::drivers::xhci) fn read_interface_descriptor<'o>(
    x: &Xhci,
    slot: u8,
    mem: &mut DeviceMem,
    kind: u8,
    iface: u8,
    out: &'o mut [u8],
) -> Result<&'o [u8], Failed> {
    let len = out.len().min(crate::mm::PAGE_SIZE) as u16;
    let got = control_in(x, slot, mem, [0x81, 6, 0, kind, iface, 0, len as u8, (len >> 8) as u8], len)?;
    Ok(data_bytes(mem, got, out))
}

pub(super) fn get_descriptor(kind: u8, index: u8, lang: u16, len: u16) -> [u8; 8] {
    [0x80, 6, index, kind, lang as u8, (lang >> 8) as u8, len as u8, (len >> 8) as u8]
}

/// The first `len` bytes of the device's data page, and no more than it holds or `out` takes.
pub(super) fn data_bytes<'o>(mem: &DeviceMem, len: u16, out: &'o mut [u8]) -> &'o [u8] {
    let len = (len as usize).min(mem.data.len()).min(out.len());
    mem.data.copy_out(0, &mut out[..len]);
    &out[..len]
}

/// **A control transfer reading `len` bytes** into the device's data page: Setup, Data, Status,
/// and a wait for the Status stage's completion or an error on any stage. **How many bytes
/// arrived**: `len` less what a short Data stage did not move.
pub(super) fn control_in(x: &Xhci, slot: u8, mem: &mut DeviceMem, request: [u8; 8], len: u16) -> Result<u16, Failed> {
    if mem.in_doubt {
        return Err(IN_DOUBT);
    }
    let len = clear_for(mem, len);
    let base = mem.ep0.phys().as_u64();
    let po = new_operation()?;
    // **Registered before the TRBs are written**, and the Status stage's address known first, so
    // no completion can come before the record it ends.
    let status_at = {
        let mut probe = mem.ring.next_slot();
        // Setup, then Data: the Status stage is two slots on, past the Link if the ring wraps.
        for _ in 0..2 {
            probe = if probe + 1 == super::RING_TRBS - 1 { 0 } else { probe + 1 };
        }
        base + probe as u64 * 16
    };
    let transfer = Awaited::Transfer { slot, status: status_at };
    let armed = armed::arm(&po, x, |c| install(&x.waiting, transfer, c))?;
    let mut slots = RingMem::of(&mem.ep0);
    mem.ring.push(&mut slots, Trb::setup(request, true));
    mem.ring.push(&mut slots, Trb::data_in(mem.data.phys().as_u64(), len as u32));
    let at = mem.ring.push(&mut slots, Trb::status(true));
    debug_assert_eq!(base + at as u64 * 16, status_at);
    x.db.write32(4 * slot as u64, DCI_EP0 as u32);
    let answer = match armed.wait(TRANSFER_NS) {
        Some((code::SUCCESS, short)) => Ok(len - short.min(len as u64) as u16),
        Some((c, _)) => Err(Failed::Code(c)),
        None => Err(Failed::Timeout),
    };
    mem.settled(answer)
}

/// **The data page cleared for a read of `len` bytes**, cut to the page: the length to ask for.
pub(super) fn clear_for(mem: &mut DeviceMem, len: u16) -> u16 {
    let len = len.min(mem.data.len() as u16);
    // `data` is a page the controller writes only during a transfer, and none is running on
    // this endpoint: every failure that can leave one live — a timeout, any error but a stall —
    // leaves it in doubt, and nothing more is asked of it; a stall's remains are skipped by
    // `recover_ep0` before another starts.
    mem.data.fill(0, len as usize, 0);
    len
}

/// **A control transfer reading one byte**: bulk-only's `GET MAX LUN`.
pub(in crate::drivers::xhci) fn control_in_byte(
    x: &Xhci,
    slot: u8,
    mem: &mut DeviceMem,
    request: [u8; 8],
) -> Result<u8, Failed> {
    control_in(x, slot, mem, request, 1)?;
    Ok(mem.data.read(0))
}

/// **A control transfer with no data stage**: Setup, then an IN Status stage, waited for.
pub(in crate::drivers::xhci) fn control_out(
    x: &Xhci,
    slot: u8,
    mem: &mut DeviceMem,
    request: [u8; 8],
) -> Result<(), Failed> {
    if mem.in_doubt {
        return Err(IN_DOUBT);
    }
    let base = mem.ep0.phys().as_u64();
    let po = new_operation()?;
    // **Registered before the TRBs are written**, as `control_in`'s: the Status stage is one slot on.
    let next = mem.ring.next_slot();
    let status_slot = if next + 1 == super::RING_TRBS - 1 { 0 } else { next + 1 };
    let status_at = base + status_slot as u64 * 16;
    let transfer = Awaited::Transfer { slot, status: status_at };
    let armed = armed::arm(&po, x, |c| install(&x.waiting, transfer, c))?;
    let mut slots = RingMem::of(&mem.ep0);
    mem.ring.push(&mut slots, Trb::setup(request, false));
    let at = mem.ring.push(&mut slots, Trb::status(false));
    debug_assert_eq!(base + at as u64 * 16, status_at);
    x.db.write32(4 * slot as u64, DCI_EP0 as u32);
    let answer = match armed.wait(TRANSFER_NS) {
        Some((code::SUCCESS, _)) => Ok(()),
        Some((c, _)) => Err(Failed::Code(c)),
        None => Err(Failed::Timeout),
    };
    mem.settled(answer)
}

/// **A control transfer writing `data`**: Setup, an OUT Data stage from the device's data page, and
/// an IN Status stage, waited for as `control_in`'s are. A keyboard's lights, as a `SET_REPORT`.
pub(in crate::drivers::xhci) fn control_out_data(
    x: &Xhci,
    slot: u8,
    mem: &mut DeviceMem,
    request: [u8; 8],
    data: &[u8],
) -> Result<(), Failed> {
    if mem.in_doubt {
        return Err(IN_DOUBT);
    }
    let len = data.len().min(crate::mm::PAGE_SIZE);
    // `data` is a page no transfer is using: the default endpoint runs one at a time, and
    // every failure that could leave one live leaves it in doubt, asked nothing more.
    mem.data.copy_in(0, &data[..len]);
    let base = mem.ep0.phys().as_u64();
    let po = new_operation()?;
    let status_at = {
        let mut probe = mem.ring.next_slot();
        for _ in 0..2 {
            probe = if probe + 1 == super::RING_TRBS - 1 { 0 } else { probe + 1 };
        }
        base + probe as u64 * 16
    };
    let transfer = Awaited::Transfer { slot, status: status_at };
    let armed = armed::arm(&po, x, |c| install(&x.waiting, transfer, c))?;
    let mut slots = RingMem::of(&mem.ep0);
    mem.ring.push(&mut slots, Trb::setup_out(request));
    mem.ring.push(&mut slots, Trb::data_out(mem.data.phys().as_u64(), len as u32));
    let at = mem.ring.push(&mut slots, Trb::status(false));
    debug_assert_eq!(base + at as u64 * 16, status_at);
    x.db.write32(4 * slot as u64, DCI_EP0 as u32);
    let answer = match armed.wait(TRANSFER_NS) {
        Some((code::SUCCESS, _)) => Ok(()),
        Some((c, _)) => Err(Failed::Code(c)),
        None => Err(Failed::Timeout),
    };
    mem.settled(answer)
}
