use super::*;
use crate::libkern::handle::KObjectType;
use crate::libkern::input::{INPUT_EVENT_LEN, KEY_RELEASE, REL_X};
use crate::mm::PAGE_SIZE;
use crate::mm::test_support::init_global_heap;
use crate::object::{MemoryObject, PendingOperation};

/// **Only a stall leaves the default endpoint to be asked again**: a stall is the request's
/// answer, and every other failure may have left the request on the ring — a transaction error (4)
/// and babble (3) halt the endpoint mid-transfer, and a timeout says nothing at all.
#[test]
fn only_a_stall_leaves_the_default_endpoint_to_ask_again() {
    assert!(!in_doubt(&Failed::Code(code::STALL)));
    for e in [Failed::Timeout, Failed::Code(4), Failed::Code(3), Failed::NoMemory, Failed::Device("short")] {
        assert!(in_doubt(&e), "{e}");
    }
}

/// **Each field where HID puts it** (HID 1.11 §7.2, USB 2.0 §9.3), read back as a device would.
/// QEMU's keyboard reads none of them — it takes any `SET_REPORT` as its lights — so a swapped
/// `wValue` would pass every gate and reach a real keyboard as an Input report with ID 2.
#[test]
#[allow(clippy::unusual_byte_groupings, reason = "grouped by the request type's fields")]
fn the_lights_request_is_an_output_report_to_its_interface() {
    let r = set_lights_request(3, None);
    assert_eq!(r[0], 0b0_01_00001, "host to device, class, to an interface");
    assert_eq!(r[1], 0x09, "SET_REPORT");
    let w_value = u16::from_le_bytes([r[2], r[3]]);
    assert_eq!(w_value >> 8, 2, "an Output report");
    assert_eq!(w_value & 0xFF, 0, "no report ID: a boot keyboard has none");
    assert_eq!(u16::from_le_bytes([r[4], r[5]]), 3, "the interface");
    assert_eq!(u16::from_le_bytes([r[6], r[7]]), 1, "one byte");
    assert_eq!(lights_data(None, 0b101), ([0b101, 0], 1), "the lights alone");

    // A keyboard whose lights are report 1, as a Corsair K70's are in report protocol: the ID in
    // `wValue`'s low byte, and first in a two-byte data stage.
    let r = set_lights_request(0, Some(1));
    let w_value = u16::from_le_bytes([r[2], r[3]]);
    assert_eq!((w_value >> 8, w_value & 0xFF), (2, 1), "an Output report, ID 1");
    assert_eq!(u16::from_le_bytes([r[6], r[7]]), 2, "the ID and the lights");
    assert_eq!(lights_data(Some(1), 0b010), ([1, 0b010], 2));
}

/// **Every batch of a change is delivered**: a bitmap keyboard pressing fifty keys and every
/// modifier in one report gives three batches, each ending in its `SYN`, and a departure lets
/// them all go the same way. A step that finds its device gone ends it, having delivered what
/// came before.
#[test]
fn every_batch_of_a_change_is_delivered() {
    use crate::drivers::hid::keyboard::{Bits, Held, Layout};
    use crate::libkern::input::{EV_SYN, KEY_RELEASE};
    let layout = Layout {
        report_id: Some(1),
        bits: [
            Some(Bits { offset: 0, first: 0xE0, count: 8 }),
            Some(Bits { offset: 8, first: 0, count: 152 }),
            None,
            None,
        ],
        array: None,
        lights: None,
    };
    let mut report = [0u8; 21];
    report[0] = 1;
    report[1] = 0xFF;
    for u in 0x04..0x04 + 50usize {
        report[2 + u / 8] |= 1 << (u % 8);
    }
    let mut held = Held::NONE;
    let mut batches: Vec<Vec<InputEvent>> = Vec::new();
    each_batch(|out| keyboard::decode(&layout, &mut held, &report, 3, out), |e| batches.push(e.to_vec()));
    let lens: Vec<usize> = batches.iter().map(Vec::len).collect();
    assert_eq!(lens, [21, 21, 19], "20, 20 and 18 presses, each with its SYN");
    assert!(batches.iter().all(|b| b.last().is_some_and(|e| e.kind == EV_SYN)));
    let pressed = batches.iter().flatten().filter(|e| e.kind == EV_KEY && e.value == KEY_PRESS).count();
    assert_eq!(pressed, 58);
    assert_eq!(held, layout.read(&report).unwrap(), "holding what the report says");

    let mut released = 0;
    each_batch(
        |out| Some(keyboard::release_all(&mut held, 4, out)),
        |e| released += e.iter().filter(|e| e.kind == EV_KEY && e.value == KEY_RELEASE).count(),
    );
    assert_eq!((released, held), (58, Held::NONE));

    let mut held = Held::NONE;
    let mut steps = 0;
    let mut delivered = 0;
    each_batch(
        |out| {
            steps += 1;
            (steps == 1).then(|| keyboard::decode(&layout, &mut held, &report, 3, out)).flatten()
        },
        |e| delivered += e.len(),
    );
    assert_eq!((steps, delivered), (2, 21), "the first batch, and then the device was gone");
}

/// **A bind's line says how its descriptor went**: its protocol, the try a descriptor read on when
/// it took more than one, and in boot protocol why. What the laptop's kernel log is read for when
/// a keyboard types the wrong keys.
#[test]
fn a_bind_line_says_how_its_descriptor_went() {
    let said = |d: Described| std::format!("{}", Said(&d));
    let none = Tries::default();
    let mut bitmap = keyboard::Layout::BOOT;
    bitmap.array = None;
    assert_eq!(said(Described::Layout(Layout::Keyboard(bitmap), none)), ", report protocol, its keys a bitmap");
    assert_eq!(
        said(Described::Layout(Layout::Keyboard(keyboard::Layout::BOOT), Tries { short: 1, stalled: 0 })),
        ", report protocol; its report descriptor read whole on try 2, after 1 short"
    );
    assert_eq!(
        said(Described::Layout(Layout::Mouse(mouse::Layout::BOOT), Tries { short: 1, stalled: 2 })),
        ", report protocol, with no wheel; its report descriptor read whole on try 4, after 1 short and 2 stalled"
    );
    assert_eq!(said(Described::None), ", boot protocol: it has no report descriptor");
    assert_eq!(
        said(Described::Unanswered { tries: Tries { short: 3, stalled: 1 }, last: Some((128, 192)) }),
        ", boot protocol: its report descriptor never came back whole in 4 tries (3 short, 1 stalled; the last \
         short one 128 of 192 bytes)"
    );
    assert_eq!(
        said(Described::Unanswered { tries: Tries { short: 0, stalled: 4 }, last: None }),
        ", boot protocol: its report descriptor never came back whole in 4 tries (0 short, 4 stalled)"
    );
    let head = [0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01];
    assert_eq!(
        said(Described::Unread { len: 192, head, tries: none, keyboard: true }),
        ", boot protocol: its 192-byte report descriptor is not one this driver reads (it begins 05 01 09 06 a1 01 85 01)"
    );
    assert_eq!(
        said(Described::Unread { len: 3, head, tries: Tries { short: 0, stalled: 2 }, keyboard: true }),
        ", boot protocol: its 3-byte report descriptor is not one this driver reads (it begins 05 01 09); its \
         report descriptor read whole on try 3, after 2 stalled",
        "no more bytes than it has"
    );
    assert_eq!(
        said(Described::Unread { len: 52, head, tries: none, keyboard: false }),
        ", boot protocol: its report descriptor does not describe a plain mouse"
    );
}

/// **A new node takes a slot no node has had, then one whose device has departed**, and none while
/// every slot is bound: sixteen bound devices, not sixteen ever attached.
#[test]
fn a_new_node_takes_a_free_slot_then_a_retired_one() {
    let mut states = [BOUND; MAX_NODES];
    assert_eq!(choose(&states), None, "every slot bound");
    states[5] = RETIRED;
    assert_eq!(choose(&states), Some(5), "a departed device's slot");
    states[9] = FREE;
    assert_eq!(choose(&states), Some(9), "a free one first");
    assert_eq!(choose(&[FREE; MAX_NODES]), Some(0));
}

/// **A context names its slot and the epoch it was made in**, so a node made before its slot was
/// reused is told apart from the one after.
#[test]
fn a_node_read_waits_out_the_delivery_before_it() {
    init_global_heap();
    let nodes = Nodes::new();
    assert_eq!(nodes.take(true), Some((0, 0)));
    input::test_support::a_read_waits_out_the_delivery_before_it(
        nodes,
        |nodes, buf, po| nodes.read(buf, po, 0, INPUT_EVENT_LEN as u64, context(0, 0), 0),
        |nodes| {
            let mut r = nodes.readers[0].lock();
            r.ring.push(InputEvent::key(30, KEY_RELEASE, 1));
            r.take_ready(&mut [0u8; DRAIN_MAX], 0).unwrap().0
        },
        |nodes, first| nodes.readers[0].lock().owe(first),
        |nodes| nodes.readers[0].lock().has_parked(),
    );
}

#[test]
fn a_context_carries_its_slot_and_epoch() {
    assert_eq!(slot_of(context(7, 0)), (7, 0));
    assert_eq!(slot_of(context(15, 3)), (15, 3));
    let (old, new) = (context(4, 1), context(4, 2));
    assert_ne!(old, new, "a reused slot's node is another");
    assert_eq!(slot_of(old).0, slot_of(new).0);
}

/// **A slot taken, retired and taken by another device serves the node it had before nothing of
/// the new one**: `take` bumps the epoch, replaces the reader and completes a lights write left
/// waiting, and `read` and `write` refuse a node from an earlier epoch. No boot reaches it — a slot
/// is reused only once all sixteen are bound — so this is its guard.
#[test]
fn a_reused_slot_serves_its_old_node_nothing_of_the_new_device() {
    init_global_heap();
    let nodes = Nodes::new();
    let mut made = [core::ptr::null_mut(); MAX_NODES];
    for (i, ctx) in made.iter_mut().enumerate() {
        let (index, epoch) = nodes.take(true).expect("a free slot");
        assert_eq!(index, i);
        *ctx = context(index, epoch);
    }
    assert_eq!(nodes.take(true), None, "every slot bound");

    // The keyboard in slot 5 departs holding a key, and a lights write is left waiting.
    nodes.retire(5, &[InputEvent::key(30, KEY_RELEASE, 1)], 1);
    // SAFETY: a fresh `PendingOperation`, adopted under its own tag.
    let lights = unsafe { crate::drivers::adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation) };
    nodes.lights.lock()[5] = Some((lights.clone(), 1));
    // A mouse takes its slot, and moves.
    let (index, epoch) = nodes.take(false).expect("the retired slot");
    assert_eq!(index, 5);
    let now = context(index, epoch);
    assert_ne!(now, made[5], "another epoch");
    assert!(nodes.lights.lock()[5].is_none(), "the waiting lights write is let go");
    // SAFETY: `lights` is a live `PendingOperation` reference held across the call.
    assert_eq!(unsafe { crate::sched::pending_op_completion(lights.as_ptr()) }, (KError::PeerClosed as i32, 0));
    nodes.readers[5].lock().ring.push(InputEvent::rel(REL_X, 3, 2));

    // SAFETY: a fresh `MemoryObject`, adopted under its own tag.
    let buf = unsafe { crate::drivers::adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject) };
    // SAFETY: a fresh `PendingOperation`, adopted under its own tag.
    let po = unsafe { crate::drivers::adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation) };
    let len = 4 * INPUT_EVENT_LEN as u64;
    assert_eq!(nodes.read(&buf, &po, 0, len, made[5], 3), Err(KError::PeerClosed), "the keyboard's node");
    assert!(!nodes.readers[5].lock().ring.is_empty(), "took none of the mouse's motion");
    assert_eq!(nodes.write(None, &buf, &po, 0, 1, made[5]), Err(KError::PeerClosed), "nor set its lights");
    assert_eq!(nodes.write(None, &buf, &po, 0, 1, now), Err(KError::Unsupported), "a mouse takes none");
    assert_eq!(nodes.read(&buf, &po, 0, len, now, 3), Ok(()), "the mouse's node reads it");
    assert_eq!(
        // SAFETY: `po` is a live `PendingOperation` reference held across the call.
        unsafe { crate::sched::pending_op_completion(po.as_ptr()) },
        (0, INPUT_EVENT_LEN as u64),
        "the motion alone: none of the keyboard's release",
    );
    assert!(nodes.current(made[4]) && nodes.current(made[6]), "the other slots' nodes are theirs");
    assert!(!nodes.current(context(MAX_NODES + 1, 0)), "no such slot");
}

/// **A report the DPC pushes after its device departed is dropped**, under the node's lock: once
/// the node has retired, so its releases stay the last thing its ring holds, and once the slot is
/// another device's, whose ring it is not.
#[test]
fn a_report_pushed_after_its_device_departed_is_dropped() {
    init_global_heap();
    let nodes = Nodes::new();
    let mut scratch = [0u8; DRAIN_MAX];
    let (index, epoch) = nodes.take(true).expect("a free slot");
    // Every other slot bound, so the next device takes this one once it retires.
    while nodes.take(false).is_some() {}
    let press = [InputEvent::key(30, KEY_PRESS, 1), InputEvent::syn(1)];
    assert!(nodes.push(index, epoch, &press, &mut scratch, 1).is_none(), "no read waiting");
    assert!(!nodes.readers[index].lock().ring.is_empty(), "pushed while the keyboard is here");

    nodes.retire(index, &[InputEvent::key(30, KEY_RELEASE, 2), InputEvent::syn(2)], 2);
    // What the ring holds, drained: how many events, and the last key among them.
    let held = |n: &Nodes| {
        let mut out = [0u8; DRAIN_MAX];
        let len = n.readers[index].lock().ring.drain_into(&mut out, 3);
        let events = out[..len].chunks(INPUT_EVENT_LEN).filter_map(InputEvent::read);
        let last = events.clone().rfind(|e| e.kind == EV_KEY).map(|e| (e.code, e.value));
        (events.count(), last)
    };
    nodes.push(index, epoch, &press, &mut scratch, 2);
    assert_eq!(held(&nodes), (4, Some((30, KEY_RELEASE))), "the press and the release, and nothing after");

    let (again, now) = nodes.take(false).expect("the retired slot");
    assert_eq!(again, index);
    nodes.push(index, epoch, &press, &mut scratch, 4);
    assert!(nodes.readers[index].lock().ring.is_empty(), "the old node's report reaches no new ring");
    nodes.push(index, now, &[InputEvent::rel(REL_X, 3, 5)], &mut scratch, 5);
    assert_eq!(held(&nodes), (1, None), "the new device's own does");
}
